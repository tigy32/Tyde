mod execution;
pub use execution::{ExecutionContext, ExecutionRoute};
pub mod pty;

use crate as process_env;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(windows)]
use std::time::Duration;
use std::time::Instant;
use tokio::io::AsyncReadExt;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

// Only runtime process handles live here. Killing never needs an actor/backend
// state lock, including when a backend is stuck shutting down.
tokio::task_local! {
    static PROCESS_OWNER: ProcessOwner;
    static WORKBENCH_PROJECT: Option<protocol::Project>;
    static EXECUTION_ENVIRONMENT: Arc<std::sync::OnceLock<Arc<ExecutionEnvironment>>>;
}

pub async fn project_scope<F: std::future::Future>(
    project: Option<protocol::Project>,
    future: F,
) -> F::Output {
    EXECUTION_ENVIRONMENT
        .scope(
            Arc::default(),
            WORKBENCH_PROJECT.scope(project, Box::pin(future)),
        )
        .await
}

pub fn project_scope_sync<T>(
    project: Option<protocol::Project>,
    operation: impl FnOnce() -> T,
) -> T {
    EXECUTION_ENVIRONMENT.sync_scope(Arc::default(), || {
        WORKBENCH_PROJECT.sync_scope(project, operation)
    })
}

pub fn current_project() -> Option<protocol::Project> {
    WORKBENCH_PROJECT.try_with(Clone::clone).ok().flatten()
}

pub fn require_nested_namespaces() -> Result<(), String> {
    let Some(project) = current_project() else {
        return Ok(());
    };
    if matches!(
        project.source,
        protocol::ProjectSource::GitWorkbench {
            environment: protocol::WorkbenchEnvironment::Bubblewrap { .. },
            ..
        }
    ) {
        let support = EXECUTION_ENVIRONMENT
            .try_with(|environment| environment.get().map(|value| value.support.clone()))
            .ok()
            .flatten()
            .ok_or("Namespace requirements need a prepared execution environment")?;
        match support.nested_namespaces {
            protocol::WorkbenchAvailability::Available => {}
            protocol::WorkbenchAvailability::Unsupported { message, .. } => return Err(message),
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct WorkspacePath {
    host: std::path::PathBuf,
    execution: std::path::PathBuf,
}

impl WorkspacePath {
    pub fn host(path: impl AsRef<std::path::Path>) -> Result<Self, String> {
        let host = path.as_ref().to_path_buf();
        let project = current_project();
        let execution = match &project {
            Some(project) => std::path::PathBuf::from(
                project
                    .runtime_path_for_host(host.to_str().ok_or("Workspace path must be UTF-8")?)?,
            ),
            None => host.clone(),
        };
        Ok(Self { host, execution })
    }

    pub fn execution(path: impl AsRef<std::path::Path>) -> Result<Self, String> {
        let execution = path.as_ref().to_path_buf();
        let project = current_project();
        let host = match &project {
            Some(project) => std::path::PathBuf::from(project.host_path_for_runtime(
                execution.to_str().ok_or("Execution path must be UTF-8")?,
            )?),
            None => execution.clone(),
        };
        Ok(Self { host, execution })
    }

    pub fn on_host(&self) -> &std::path::Path {
        &self.host
    }

    pub fn in_execution(&self) -> &std::path::Path {
        &self.execution
    }

    pub fn execution_string(&self) -> Result<String, String> {
        self.execution
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| "Execution path must be UTF-8".to_owned())
    }
}

#[derive(serde::Deserialize, PartialEq, Eq)]
pub struct ExecutionEnvironment {
    pub support: protocol::WorkbenchSupport,
    pub values: std::collections::BTreeMap<String, String>,
    pub configuration_identity: String,
    pub runner_identity: String,
}

pub fn set_execution_environment(values: ExecutionEnvironment) -> Result<(), String> {
    EXECUTION_ENVIRONMENT
        .try_with(|environment| match environment.set(Arc::new(values)) {
            Ok(()) => Ok(()),
            Err(values) if environment.get() == Some(&values) => Ok(()),
            Err(_) => Err("Execution environment changed during the workspace lifetime".to_owned()),
        })
        .map_err(|_| "Execution environment has no workspace owner".to_owned())?
}

pub fn environment_var(key: &str) -> Result<String, std::env::VarError> {
    EXECUTION_ENVIRONMENT
        .try_with(|environment| {
            environment.get().map(|environment| {
                environment
                    .values
                    .get(key)
                    .cloned()
                    .ok_or(std::env::VarError::NotPresent)
            })
        })
        .ok()
        .flatten()
        .unwrap_or_else(|| std::env::var(key))
}

pub fn environment_var_os(key: &str) -> Option<std::ffi::OsString> {
    EXECUTION_ENVIRONMENT
        .try_with(|environment| {
            environment
                .get()
                .map(|environment| environment.values.get(key).map(std::ffi::OsString::from))
        })
        .ok()
        .flatten()
        .unwrap_or_else(|| std::env::var_os(key))
}

type SharedProcess = Arc<std::sync::Mutex<OwnedProcess>>;

#[derive(Clone, Default)]
pub struct ProcessOwner {
    state: Arc<std::sync::Mutex<ProcessOwnerState>>,
    parent: Option<Arc<ProcessOwner>>,
}

#[derive(Default)]
struct ProcessOwnerState {
    stopped: bool,
    launches: Vec<std::sync::Weak<PendingLaunch>>,
    children: Vec<std::sync::Weak<std::sync::Mutex<OwnedProcess>>>,
}

struct OwnedProcess {
    child: Option<NativeChild>,
    group_signalled: bool,
    handed_off: bool,
}

// Raw native ownership never leaves the spawning thread, even on unwinding.
struct NativeProcessGuard(SharedProcess);
impl Drop for NativeProcessGuard {
    fn drop(&mut self) {
        let child = self
            .0
            .lock()
            .expect("process signal mutex poisoned")
            .child
            .take();
        drop(child);
    }
}

static NEXT_LAUNCH_TICKET: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug)]
enum LaunchPhase {
    Registered,
    WorkerStarted,
    Failed,
    SpawnStarted,
    SpawnReturned,
    Cancelled,
    CleanupStarted,
    CleanupFinished,
    CleanupFailed,
    HandoffRequested,
    HandoffAuthorized,
    #[cfg(unix)]
    HelperPrepared,
}

struct PendingLaunch {
    ticket: u64,
    started: Instant,
    cancellation: CancellationToken,
    #[cfg(all(feature = "test-support", target_os = "linux"))]
    probe: Option<test_support::Probe>,
}

impl PendingLaunch {
    fn trace(&self, phase: LaunchPhase) {
        tracing::info!(
            launch_ticket = self.ticket,
            ?phase,
            elapsed_ms = %self.started.elapsed().as_millis(),
            "backend process launch"
        );
        #[cfg(feature = "test-support")]
        if std::env::var_os("TYDE_DIAGNOSE_PROCESS_PHASES").is_some() {
            eprintln!(
                "Native launch diagnostic: ticket={} phase={phase:?}",
                self.ticket
            );
        }
        #[cfg(all(feature = "test-support", target_os = "linux"))]
        if let Some(probe) = &self.probe {
            probe.phase(phase);
        }
    }

    fn cancel(&self) {
        self.cancellation.cancel();
        self.trace(LaunchPhase::Cancelled);
    }
}

struct PendingLaunchGuard(Option<Arc<PendingLaunch>>);

impl Drop for PendingLaunchGuard {
    fn drop(&mut self) {
        if let Some(launch) = self.0.take() {
            launch.cancel();
        }
    }
}

fn cancelled_launch_error() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Interrupted, "backend launch cancelled")
}

impl OwnedProcess {
    fn kill(&mut self) -> std::io::Result<()> {
        if self.group_signalled || self.handed_off {
            return Ok(());
        }
        if let Some(child) = self.child.as_mut() {
            match child.start_kill() {
                Ok(()) => {}
                #[cfg(unix)]
                Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {}
                Err(error) => return Err(error),
            }
        }
        self.group_signalled = true;
        Ok(())
    }
}

impl ProcessOwner {
    /// A cancellable startup subtree still belongs to its enclosing Backend.
    pub fn child() -> Self {
        Self {
            parent: PROCESS_OWNER.try_with(|owner| Arc::new(owner.clone())).ok(),
            ..Self::default()
        }
    }
    pub fn scope<F: std::future::Future>(
        &self,
        future: F,
    ) -> impl std::future::Future<Output = F::Output> {
        tracing::debug!(
            future_bytes = std::mem::size_of::<F>(),
            "backend process-owner scope"
        );
        // Task-local wrappers must not duplicate large actor futures on the
        // caller's stack in debug builds.
        PROCESS_OWNER.scope(self.clone(), Box::pin(future))
    }

    pub fn kill_all(&self) {
        let (launches, children) = {
            let mut state = self.state.lock().expect("process owner mutex poisoned");
            state.stopped = true;
            let launches: Vec<_> = state
                .launches
                .drain(..)
                .filter_map(|launch| launch.upgrade())
                .collect();
            let children: Vec<_> = state
                .children
                .drain(..)
                .filter_map(|child| child.upgrade())
                .collect();
            (launches, children)
        };
        for launch in launches {
            launch.cancel();
        }
        for child in children {
            if let Err(error) = child.lock().expect("process signal mutex poisoned").kill() {
                tracing::error!(%error, "failed to kill owned backend process group");
            }
        }
    }

    fn register_launch(&self, launch: &Arc<PendingLaunch>) -> std::io::Result<()> {
        if let Some(parent) = &self.parent {
            parent.register_launch(launch)?;
        }
        let mut state = self.state.lock().expect("process owner mutex poisoned");
        if state.stopped {
            return Err(cancelled_launch_error());
        }
        state.launches.retain(|launch| launch.strong_count() != 0);
        state.launches.push(Arc::downgrade(launch));
        Ok(())
    }

    fn register_child(&self, process: &SharedProcess) -> bool {
        if self
            .parent
            .as_ref()
            .is_some_and(|parent| !parent.register_child(process))
        {
            return false;
        }
        let mut state = self.state.lock().expect("process owner mutex poisoned");
        if state.stopped {
            return false;
        }
        state.children.retain(|child| child.strong_count() != 0);
        state.children.push(Arc::downgrade(process));
        true
    }

    fn finish_launch(&self, launch: &Arc<PendingLaunch>) -> bool {
        let accepted = {
            let mut state = self.state.lock().expect("process owner mutex poisoned");
            state.launches.retain(|pending| {
                pending
                    .upgrade()
                    .is_some_and(|pending| !Arc::ptr_eq(&pending, launch))
            });
            !state.stopped
        };
        self.parent
            .as_ref()
            .is_none_or(|parent| parent.finish_launch(launch))
            && accepted
    }
}

pub fn spawn<F>(future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    tokio::spawn(inherit_execution(future))
}

pub fn spawn_blocking<F, T>(operation: F) -> tokio::task::JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let owner = PROCESS_OWNER.try_with(Clone::clone).ok();
    let project = current_project();
    let environment = EXECUTION_ENVIRONMENT
        .try_with(Arc::clone)
        .unwrap_or_default();
    #[cfg(all(feature = "test-support", target_os = "linux"))]
    let probe = test_support::CURRENT_PROBE.try_with(Clone::clone).ok();
    tokio::task::spawn_blocking(move || {
        let operation = || {
            EXECUTION_ENVIRONMENT.sync_scope(environment, || {
                WORKBENCH_PROJECT.sync_scope(project, operation)
            })
        };
        let operation = || match owner {
            Some(owner) => PROCESS_OWNER.sync_scope(owner, operation),
            None => operation(),
        };
        #[cfg(all(feature = "test-support", target_os = "linux"))]
        {
            match probe {
                Some(probe) => test_support::CURRENT_PROBE.sync_scope(probe, operation),
                None => operation(),
            }
        }
        #[cfg(not(all(feature = "test-support", target_os = "linux")))]
        operation()
    })
}

pub fn inherit_execution<F>(future: F) -> impl std::future::Future<Output = F::Output> + Send
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    let future = Box::pin(future);
    let owner = PROCESS_OWNER.try_with(Clone::clone).ok();
    let workbench = WORKBENCH_PROJECT.try_with(Clone::clone).ok().flatten();
    let environment = EXECUTION_ENVIRONMENT
        .try_with(Arc::clone)
        .unwrap_or_default();
    #[cfg(all(feature = "test-support", target_os = "linux"))]
    let probe = test_support::CURRENT_PROBE.try_with(Clone::clone).ok();
    async move {
        let future =
            EXECUTION_ENVIRONMENT.scope(environment, WORKBENCH_PROJECT.scope(workbench, future));
        #[cfg(all(feature = "test-support", target_os = "linux"))]
        let future = async move {
            match probe {
                Some(probe) => test_support::CURRENT_PROBE.scope(probe, future).await,
                None => future.await,
            }
        };
        match owner {
            Some(owner) => owner.scope(future).await,
            None => future.await,
        }
    }
}

pub struct AsyncGroupChild {
    process: SharedProcess,
    cleanup: Option<oneshot::Sender<()>>,
    exit: tokio::sync::watch::Receiver<ProcessExit>,
    execution: Option<ExecutionContext>,
    #[cfg(target_os = "linux")]
    execution_plan: Option<Arc<std::fs::File>>,
}

impl std::fmt::Debug for AsyncGroupChild {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AsyncGroupChild")
            .finish_non_exhaustive()
    }
}

impl AsyncGroupChild {
    fn with_child<T>(&self, action: impl FnOnce(&mut NativeChild) -> T) -> T {
        let mut process = self.process.lock().expect("process signal mutex poisoned");
        action(process.child.as_mut().expect("owned child already taken"))
    }

    pub fn take_stdin(&mut self) -> Option<tokio::process::ChildStdin> {
        self.with_child(|child| child.take_stdin())
    }
    pub fn take_stdout(&mut self) -> Option<tokio::process::ChildStdout> {
        self.with_child(|child| child.take_stdout())
    }
    pub fn take_stderr(&mut self) -> Option<tokio::process::ChildStderr> {
        self.with_child(|child| child.take_stderr())
    }
    pub fn id(&self) -> Option<u32> {
        self.with_child(|child| child.id())
    }
    pub fn start_kill(&mut self) -> std::io::Result<()> {
        self.process
            .lock()
            .expect("process signal mutex poisoned")
            .kill()
    }
    pub fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        let mut process = self.process.lock().expect("process signal mutex poisoned");
        let status = process
            .child
            .as_mut()
            .expect("owned child already taken")
            .try_wait()?;
        if status.is_some() {
            // A leader can exit while descendants still hold pipes. Kill its
            // group before releasing the ownership guard, on Unix and Windows.
            process.kill()?;
        }
        Ok(status)
    }
    pub async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        #[cfg(unix)]
        let ready = self.with_child(|child| child.readiness());
        // Signals and cancellation never wait for the child's exit future.
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(status);
            }
            #[cfg(unix)]
            ready.wait().await?;
            #[cfg(windows)]
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    pub async fn kill(&mut self) -> std::io::Result<()> {
        self.start_kill()?;
        self.wait().await.map(|_| ())
    }

    pub async fn wait_with_output(mut self) -> std::io::Result<std::process::Output> {
        drop(self.take_stdin());
        async fn read_output<R: tokio::io::AsyncRead + Unpin>(
            stream: Option<R>,
        ) -> std::io::Result<Vec<u8>> {
            let mut bytes = Vec::new();
            if let Some(mut stream) = stream {
                stream.read_to_end(&mut bytes).await?;
            }
            Ok(bytes)
        }
        let stdout = self.take_stdout();
        let stderr = self.take_stderr();
        let (status, stdout_bytes, stderr_bytes) =
            tokio::try_join!(self.wait(), read_output(stdout), read_output(stderr))?;
        Ok(std::process::Output {
            status,
            stdout: stdout_bytes,
            stderr: stderr_bytes,
        })
    }
}

#[cfg(unix)]
impl command_group::UnixChildExt for AsyncGroupChild {
    fn signal(&self, signal: command_group::Signal) -> std::io::Result<()> {
        self.with_child(|child| child.signal(signal))
    }
}

impl Drop for AsyncGroupChild {
    fn drop(&mut self) {
        if self.cleanup.is_none() {
            return;
        }
        if let Err(error) = self
            .process
            .lock()
            .expect("process signal mutex poisoned")
            .kill()
        {
            tracing::error!(%error, "backend group drop kill failed");
        }
        if let Some(cleanup) = self.cleanup.take() {
            let _ = cleanup.send(());
        }
        #[cfg(target_os = "linux")]
        drop(self.execution_plan.take());
    }
}

#[derive(Clone)]
pub enum ProcessStdio {
    Inherit,
    Null,
    Piped,
    File(Arc<std::fs::File>),
}
impl ProcessStdio {
    pub fn piped() -> Self {
        Self::Piped
    }
    pub fn null() -> Self {
        Self::Null
    }
    pub fn inherit() -> Self {
        Self::Inherit
    }
}
impl From<std::fs::File> for ProcessStdio {
    fn from(file: std::fs::File) -> Self {
        Self::File(Arc::new(file))
    }
}

#[derive(Clone, Copy)]
#[repr(u8)]
pub(crate) enum SessionMode {
    Group = 0,
    New = 1,
    #[cfg(unix)]
    Terminal = 2,
}

#[derive(Clone, Copy)]
#[repr(u8)]
pub enum IndependentProcess {
    ManagedHost = 1,
    ExternalApplication = 2,
}

#[derive(Clone)]
pub struct GroupCommand {
    program: std::ffi::OsString,
    args: Vec<std::ffi::OsString>,
    env: Vec<(std::ffi::OsString, Option<std::ffi::OsString>)>,
    cwd: Option<std::path::PathBuf>,
    stdio: [Option<ProcessStdio>; 3],
    session: SessionMode,
    independent: Option<IndependentProcess>,
    workbench: Option<protocol::Project>,
    prepared_environment: Option<Arc<ExecutionEnvironment>>,
    capture_execution: bool,
    #[cfg(target_os = "linux")]
    active_execution: Option<ExecutionContext>,
    #[cfg(target_os = "linux")]
    execution_plan: Option<Arc<std::fs::File>>,
    #[cfg(all(feature = "test-support", target_os = "linux"))]
    probe: Option<test_support::Probe>,
}

pub fn process_path() -> Result<std::ffi::OsString, String> {
    match EXECUTION_ENVIRONMENT
        .try_with(|environment| environment.get().cloned())
        .ok()
        .flatten()
    {
        Some(environment) => environment
            .values
            .get("PATH")
            .map(std::ffi::OsString::from)
            .ok_or_else(|| "Execution environment is missing PATH".to_owned()),
        None => process_env::initialize_process_env().map(std::ffi::OsString::from),
    }
}

pub fn host_command(program: impl AsRef<std::ffi::OsStr>) -> Result<GroupCommand, String> {
    let mut command = raw_command(program);
    command
        .on_host()
        .env("PATH", process_env::initialize_process_env()?);
    Ok(command)
}

pub fn command(program: impl AsRef<std::ffi::OsStr>) -> Result<GroupCommand, String> {
    let mut command = raw_command(program);
    command.env("PATH", process_path()?);
    Ok(command)
}

pub(crate) fn raw_command(program: impl AsRef<std::ffi::OsStr>) -> GroupCommand {
    GroupCommand {
        program: program.as_ref().to_owned(),
        args: Vec::new(),
        env: Vec::new(),
        cwd: None,
        stdio: [const { None }; 3],
        session: SessionMode::Group,
        independent: None,
        workbench: WORKBENCH_PROJECT.try_with(Clone::clone).ok().flatten(),
        capture_execution: false,
        #[cfg(target_os = "linux")]
        active_execution: None,
        #[cfg(target_os = "linux")]
        execution_plan: None,
        prepared_environment: EXECUTION_ENVIRONMENT
            .try_with(|environment| environment.get().cloned())
            .ok()
            .flatten(),
        #[cfg(all(feature = "test-support", target_os = "linux"))]
        probe: test_support::current_probe(),
    }
}

impl GroupCommand {
    pub fn in_project(&mut self, project: protocol::Project) -> &mut Self {
        if self.workbench.as_ref() != Some(&project) {
            self.prepared_environment = None;
        }
        self.workbench = Some(project);
        self
    }

    pub(crate) fn on_host(&mut self) -> &mut Self {
        self.workbench = None;
        self.prepared_environment = None;
        self
    }

    fn wrap_workbench(
        mut self,
        #[cfg(target_os = "linux")] setup: Option<&execution::Setup>,
    ) -> std::io::Result<Self> {
        #[cfg(target_os = "linux")]
        if let Some(execution) = self.active_execution.clone() {
            return execution.wrap(self);
        }
        let Some(project) = self.workbench.take() else {
            return Ok(self);
        };
        let protocol::ProjectSource::GitWorkbench {
            roots,
            environment:
                protocol::WorkbenchEnvironment::Bubblewrap {
                    manifest_path,
                    logical_root,
                    ..
                },
            ..
        } = &project.source
        else {
            return Ok(self);
        };
        project
            .execution_root_mappings()
            .map(|_| ())
            .map_err(std::io::Error::other)?;
        if !cfg!(target_os = "linux") {
            return Err(std::io::Error::other("Bubblewrap requires Linux"));
        }
        let root = std::path::Path::new(&roots[0].worktree_root.0);
        let cwd = match &self.cwd {
            Some(cwd) => std::path::PathBuf::from(
                project
                    .runtime_path_for_host(
                        cwd.to_str()
                            .ok_or_else(|| std::io::Error::other("Workspace path must be UTF-8"))?,
                    )
                    .map_err(std::io::Error::other)?,
            ),
            None => std::path::PathBuf::from(&logical_root.0),
        };
        // The launcher applies cwd before the namespace exists. Only the
        // runner's inner --chdir may use the stable logical mount.
        self.cwd = Some(root.to_path_buf());
        let helper = std::path::Path::new(&manifest_path.0).with_file_name("runner.py");
        let prepared = self.prepared_environment.take();
        if let Some(environment) = &prepared {
            let mut base = std::env::vars_os()
                .map(|(key, _)| (key, None))
                .collect::<Vec<_>>();
            base.extend(
                environment
                    .values
                    .iter()
                    .map(|(key, value)| (key.into(), Some(value.into()))),
            );
            base.append(&mut self.env);
            self.env = base;
        }
        let mut args = vec![
            helper.into_os_string(),
            match &prepared {
                Some(_) if self.capture_execution => "launch_prepared_context",
                None if self.capture_execution => "launch_context",
                Some(_) => "launch_prepared",
                None => "launch",
            }
            .into(),
            manifest_path.0.as_str().into(),
            root.as_os_str().to_owned(),
            cwd.into_os_string(),
            self.program,
        ];
        if let Some(environment) = prepared {
            args.insert(5, environment.configuration_identity.as_str().into());
            args.insert(6, environment.runner_identity.as_str().into());
        }
        #[cfg(target_os = "linux")]
        if let Some(setup) = setup {
            args.insert(5, setup.address().into_os_string());
        }
        args.append(&mut self.args);
        self.program = process_env::find_host_executable_in_path("python3")
            .ok_or_else(|| std::io::Error::other("Bubblewrap runtime requires host Python 3"))?
            .into_os_string();
        self.args = args;
        Ok(self)
    }

    pub async fn group_spawn_with_execution(
        mut self,
    ) -> std::io::Result<(AsyncGroupChild, ExecutionContext)> {
        self.capture_execution = true;
        let child = self.group_spawn().await?;
        let execution = child.execution.clone().ok_or_else(|| {
            std::io::Error::other("Native launch did not provide its execution context")
        })?;
        Ok((child, execution))
    }

    #[cfg(unix)]
    pub(crate) fn new_session(&mut self) -> &mut Self {
        self.session = SessionMode::New;
        self
    }
    pub async fn output(&mut self) -> std::io::Result<std::process::Output> {
        for (mode, default) in self.stdio.iter_mut().zip([
            ProcessStdio::Null,
            ProcessStdio::Piped,
            ProcessStdio::Piped,
        ]) {
            mode.get_or_insert(default);
        }
        let command = self.clone();
        #[cfg(all(feature = "test-support", target_os = "linux"))]
        {
            self.probe = None;
        }
        command.group_spawn().await?.wait_with_output().await
    }
    pub fn program(&self) -> &std::ffi::OsStr {
        &self.program
    }
    pub fn arg(&mut self, arg: impl AsRef<std::ffi::OsStr>) -> &mut Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }
    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|arg| arg.as_ref().to_owned()));
        self
    }
    pub fn env(
        &mut self,
        key: impl AsRef<std::ffi::OsStr>,
        value: impl AsRef<std::ffi::OsStr>,
    ) -> &mut Self {
        self.env
            .push((key.as_ref().to_owned(), Some(value.as_ref().to_owned())));
        self
    }
    pub fn envs<I, K, V>(&mut self, env: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<std::ffi::OsStr>,
        V: AsRef<std::ffi::OsStr>,
    {
        for (key, value) in env {
            self.env(key, value);
        }
        self
    }
    pub fn env_remove(&mut self, key: impl AsRef<std::ffi::OsStr>) -> &mut Self {
        self.env.push((key.as_ref().to_owned(), None));
        self
    }
    pub fn current_dir(&mut self, path: impl AsRef<std::path::Path>) -> &mut Self {
        self.cwd = Some(path.as_ref().to_owned());
        self
    }
    pub fn stdin(&mut self, mode: impl Into<ProcessStdio>) -> &mut Self {
        self.stdio[0] = Some(mode.into());
        self
    }
    pub fn stdout(&mut self, mode: impl Into<ProcessStdio>) -> &mut Self {
        self.stdio[1] = Some(mode.into());
        self
    }
    pub fn stderr(&mut self, mode: impl Into<ProcessStdio>) -> &mut Self {
        self.stdio[2] = Some(mode.into());
        self
    }
}

pub trait AsyncCommandGroup {
    fn group_spawn(
        self,
    ) -> impl std::future::Future<Output = std::io::Result<AsyncGroupChild>> + Send;
}

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix::Child as NativeChild;
#[cfg(unix)]
pub use unix::exec_helper_entry;
#[cfg(all(feature = "test-support", target_os = "linux"))]
pub mod test_support;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows::Child as NativeChild;

impl AsyncCommandGroup for GroupCommand {
    async fn group_spawn(self) -> std::io::Result<AsyncGroupChild> {
        #[cfg(unix)]
        {
            launch(self, None).await
        }
        #[cfg(windows)]
        {
            launch(self).await
        }
    }
}
async fn launch(
    spec: GroupCommand,
    #[cfg(unix)] admission: Option<DeferredPlan>,
) -> std::io::Result<AsyncGroupChild> {
    #[cfg(target_os = "linux")]
    let setup = if spec.capture_execution && spec.active_execution.is_none() {
        match spec.workbench.as_ref().map(|project| &project.source) {
            Some(protocol::ProjectSource::GitWorkbench {
                environment: protocol::WorkbenchEnvironment::Bubblewrap { manifest_path, .. },
                ..
            }) => {
                let python =
                    process_env::find_host_executable_in_path("python3").ok_or_else(|| {
                        std::io::Error::other("Execution context requires host Python 3")
                    })?;
                Some(execution::Setup::new(
                    std::path::Path::new(&manifest_path.0),
                    python,
                )?)
            }
            _ => None,
        }
    } else {
        None
    };
    let captured = spec.capture_execution;
    #[cfg(target_os = "linux")]
    let active = spec.active_execution.clone();
    #[cfg(target_os = "linux")]
    let spec = spec.wrap_workbench(setup.as_ref())?;
    #[cfg(not(target_os = "linux"))]
    let spec = spec.wrap_workbench()?;
    #[cfg(target_os = "linux")]
    let execution_plan = spec.execution_plan.clone();
    let owner = PROCESS_OWNER.try_with(Clone::clone).ok();
    let launch = Arc::new(PendingLaunch {
        ticket: NEXT_LAUNCH_TICKET.fetch_add(1, Ordering::Relaxed),
        started: Instant::now(),
        cancellation: CancellationToken::new(),
        #[cfg(all(feature = "test-support", target_os = "linux"))]
        probe: spec.probe.clone(),
    });
    if let Some(owner) = &owner {
        owner.register_launch(&launch)?;
    }
    launch.trace(LaunchPhase::Registered);
    let mut guard = PendingLaunchGuard(Some(Arc::clone(&launch)));
    let (reply_tx, mut reply_rx) = oneshot::channel();
    let (commit_tx, mut commit_rx) = oneshot::channel::<oneshot::Sender<()>>();
    let worker_launch = Arc::clone(&launch);
    let worker_owner = owner.clone();
    // The first helper exec can itself stall in libc/kernel startup. Its
    // owning thread survives cancellation and reaps any late native return.
    std::thread::Builder::new().name("tyde-backend-launch".to_owned()).spawn(move || {
            worker_launch.trace(LaunchPhase::WorkerStarted);
            #[cfg(all(feature = "test-support", target_os = "linux"))]
            if let Some(probe) = &worker_launch.probe
                && let Err(error) = probe.install()
            {
                let _ = reply_tx.send(Err(error));
                return;
            }
            let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(runtime) => runtime,
                Err(error) => { worker_launch.trace(LaunchPhase::Failed); let _ = reply_tx.send(Err(error)); return; }
            };
            // PDEATHSIG follows this thread, which also drives the child's pipes.
            runtime.block_on(async move {
                #[cfg(target_os = "linux")]
                let execution_resources = (spec.active_execution.clone(), spec.execution_plan.clone());
                if worker_launch.cancellation.is_cancelled() { let _ = reply_tx.send(Err(cancelled_launch_error())); return; }
                worker_launch.trace(LaunchPhase::SpawnStarted);
                #[cfg(unix)]
                let spawned = unix::spawn(spec);
                #[cfg(windows)]
                let spawned = windows::spawn(spec);
                worker_launch.trace(LaunchPhase::SpawnReturned);
                #[cfg(unix)]
                let (child,exchange) = match spawned {
                    Ok(child) => child,
                    Err(error) => { worker_launch.trace(LaunchPhase::Failed); let _ = reply_tx.send(Err(error)); return; }
                };
                #[cfg(windows)]
                drop(commit_tx);
                #[cfg(windows)]
                let child = match spawned {
                    Ok(child) => child,
                    Err(error) => { worker_launch.trace(LaunchPhase::Failed); let _ = reply_tx.send(Err(error)); return; }
                };
                tracing::info!(launch_ticket=worker_launch.ticket,child_pid=child.id(),"backend native process ownership established");
                #[cfg(all(feature = "test-support", target_os = "linux"))]
                if let Some(probe) = &worker_launch.probe { probe.child(child.id()); }
                let (release,cleanup) = oneshot::channel();
                let (exit_tx,exit)=tokio::sync::watch::channel(ProcessExit::Running);
                let process = Arc::new(std::sync::Mutex::new(OwnedProcess {
                    child: Some(child), group_signalled: false, handed_off: false,
                }));
                let native_guard = NativeProcessGuard(Arc::clone(&process));
                let registered = match &worker_owner { Some(owner) => owner.register_child(&process), None => true };
                let child = AsyncGroupChild { process, cleanup: Some(release), exit, execution: None, #[cfg(target_os = "linux")] execution_plan: None };
                let result = if !registered || worker_launch.cancellation.is_cancelled() {
                    Err(cancelled_launch_error())
                } else {
                    #[cfg(unix)]
                    { tokio::select! {
                        biased;
                        _ = worker_launch.cancellation.cancelled() => Err(cancelled_launch_error()),
                        result = exchange.complete(commit_tx, Arc::clone(&worker_launch), admission) => result,
                    } }
                    #[cfg(windows)]
                    { Ok(()) }
                };
                match result {
                    Ok(()) if !worker_launch.cancellation.is_cancelled() => { let _ = reply_tx.send(Ok(child)); }
                    Ok(()) => { drop(child); let _ = reply_tx.send(Err(cancelled_launch_error())); }
                    Err(error) => {
                        worker_launch.trace(LaunchPhase::Failed);
                        tracing::error!(launch_ticket=worker_launch.ticket,error_kind=?error.kind(),"backend executable startup failed");
                        drop(child);
                        let _ = reply_tx.send(Err(error));
                    }
                }
                let _ = cleanup.await;
                let child = native_guard.0.lock().expect("process signal mutex poisoned").child.take();
                if let Some(mut child) = child {
                    worker_launch.trace(LaunchPhase::CleanupStarted);
                    match child.wait().await {
                        Ok(status) => { exit_tx.send_replace(ProcessExit::Exited(status)); worker_launch.trace(LaunchPhase::CleanupFinished); },
                        Err(error) => {
                            exit_tx.send_replace(ProcessExit::Failed(Arc::new(std::io::Error::new(error.kind(),error.to_string()))));
                            worker_launch.trace(LaunchPhase::CleanupFailed);
                            tracing::error!(launch_ticket=worker_launch.ticket,error_kind=?error.kind(),"backend group reaping failed");
                        }
                    }
                }
                #[cfg(target_os = "linux")]
                drop(execution_resources);
            });
        })?;
    let mut commit_pending = true;
    let result = loop {
        tokio::select! {
            biased;
            _ = launch.cancellation.cancelled() => break Err(cancelled_launch_error()),
            result = &mut commit_rx, if commit_pending => {
                commit_pending=false;
                if let Ok(grant) = result {
                    launch.trace(LaunchPhase::HandoffRequested);
                    if !launch.cancellation.is_cancelled() {
                        let _ = grant.send(());
                        launch.trace(LaunchPhase::HandoffAuthorized);
                    }
                }
            }
            result = &mut reply_rx => break result.map_err(|_| std::io::Error::other("backend launch worker closed without a result"))?,
        }
    };
    // Keep cancellation registered until the receiving task accepts ownership.
    let accepted = owner
        .as_ref()
        .is_none_or(|owner| owner.finish_launch(&launch));
    guard.0 = None;
    if !accepted || launch.cancellation.is_cancelled() {
        return Err(cancelled_launch_error());
    }
    let mut child = result?;
    #[cfg(target_os = "linux")]
    {
        child.execution_plan = execution_plan;
    }
    #[cfg(target_os = "linux")]
    {
        let execution = match (setup, active) {
            (Some(setup), None) => Some(setup.receive(&mut child).await?),
            (None, Some(execution)) => Some(execution),
            (None, None) if captured => Some(ExecutionContext::default()),
            (None, None) => None,
            (Some(_), Some(_)) => {
                return Err(std::io::Error::other(
                    "Native launch has conflicting execution owners",
                ));
            }
        };
        child.execution = execution;
    }
    #[cfg(not(target_os = "linux"))]
    if captured {
        child.execution = Some(ExecutionContext::default());
    }
    Ok(child)
}
/// Blocking callers use the same native launch/ownership engine, never std spawn.
pub struct BlockingChild {
    child: AsyncGroupChild,
    pub stdin: Option<std::fs::File>,
}
impl BlockingChild {
    pub fn id(&self) -> Option<u32> {
        self.child.id()
    }
    pub fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.child.try_wait()
    }
    pub fn kill(&mut self) -> std::io::Result<()> {
        self.child.start_kill()
    }
    pub fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        drop(self.stdin.take());
        blocking_process(self.child.wait())
    }
    pub fn wait_with_output(mut self) -> std::io::Result<std::process::Output> {
        drop(self.stdin.take());
        blocking_process(self.child.wait_with_output())
    }
}
fn blocking_process<F, T>(future: F) -> std::io::Result<T>
where
    F: std::future::Future<Output = std::io::Result<T>> + Send,
    T: Send,
{
    let owner = PROCESS_OWNER.try_with(Clone::clone).ok();
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name("tyde-process-blocking-caller".to_owned())
            .spawn_scoped(scope, move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                match owner {
                    Some(owner) => runtime.block_on(owner.scope(future)),
                    None => runtime.block_on(future),
                }
            })?
            .join()
            .map_err(|_| std::io::Error::other("blocking process caller failed"))?
    })
}

impl GroupCommand {
    pub fn spawn_blocking(&mut self) -> std::io::Result<BlockingChild> {
        let mut child = blocking_process(self.clone().group_spawn())?;
        #[cfg(unix)]
        let stdin = child
            .take_stdin()
            .map(|stdin| {
                use std::os::fd::AsRawFd;
                let fd = stdin.into_owned_fd()?;
                let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
                if flags < 0
                    || unsafe {
                        libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK)
                    } < 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(std::fs::File::from(fd))
            })
            .transpose()?;
        #[cfg(windows)]
        let stdin = child
            .take_stdin()
            .map(|stdin| stdin.into_owned_handle().map(std::fs::File::from))
            .transpose()?;
        Ok(BlockingChild { child, stdin })
    }
    pub fn output_blocking(&mut self) -> std::io::Result<std::process::Output> {
        blocking_process(async { self.clone().output().await })
    }
    pub fn status_blocking(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.spawn_blocking()?.wait()
    }
}

pub struct OwnedChildOutput {
    child: AsyncGroupChild,
    output: tokio::process::ChildStdout,
}
impl tokio::io::AsyncRead for OwnedChildOutput {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().output).poll_read(context, buffer)
    }
}
impl Drop for OwnedChildOutput {
    fn drop(&mut self) {
        if let Err(error) = self.child.start_kill() {
            tracing::error!(error_kind=?error.kind(), "stdio transport process cleanup failed");
        }
    }
}
impl AsyncGroupChild {
    pub fn into_owned_pipes(
        mut self,
    ) -> std::io::Result<(OwnedChildOutput, tokio::process::ChildStdin)> {
        if self.take_stderr().is_some() {
            return Err(std::io::Error::other(
                "owned pipe transport requires inherited or null stderr",
            ));
        }
        let input = self
            .take_stdin()
            .ok_or_else(|| std::io::Error::other("process stdin was not piped"))?;
        let output = self
            .take_stdout()
            .ok_or_else(|| std::io::Error::other("process stdout was not piped"))?;
        Ok((
            OwnedChildOutput {
                child: self,
                output,
            },
            input,
        ))
    }
}

impl GroupCommand {
    /// Transfer lifetime deliberately, only for a managed host or external app.
    /// Until the live caller grants the helper's commit, normal ownership holds.
    pub async fn launch_independent(
        mut self,
        purpose: IndependentProcess,
    ) -> std::io::Result<IndependentChild> {
        if self
            .stdio
            .iter()
            .any(|mode| matches!(mode, Some(ProcessStdio::Piped)))
        {
            return Err(std::io::Error::other(
                "independent process cannot own caller pipes",
            ));
        }
        self.independent = Some(purpose);
        self.session = SessionMode::New;
        finish_handoff(self.group_spawn().await?)
    }
    pub fn launch_independent_blocking(
        &mut self,
        purpose: IndependentProcess,
    ) -> std::io::Result<IndependentChild> {
        blocking_process(self.clone().launch_independent(purpose))
    }
}

#[derive(Clone)]
enum ProcessExit {
    Running,
    Exited(std::process::ExitStatus),
    Failed(Arc<std::io::Error>),
}
pub struct IndependentChild {
    exit: tokio::sync::watch::Receiver<ProcessExit>,
}
impl IndependentChild {
    pub async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        loop {
            let state = self.exit.borrow_and_update().clone();
            match state {
                ProcessExit::Running => {}
                ProcessExit::Exited(status) => return Ok(status),
                ProcessExit::Failed(error) => {
                    return Err(std::io::Error::new(error.kind(), error.to_string()));
                }
            }
            self.exit.changed().await.map_err(|_| {
                std::io::Error::other("independent process owner ended without exit status")
            })?;
        }
    }
}

fn finish_handoff(mut child: AsyncGroupChild) -> std::io::Result<IndependentChild> {
    {
        let mut process = child.process.lock().expect("process signal mutex poisoned");
        if process.group_signalled {
            return Err(cancelled_launch_error());
        }
        process
            .child
            .as_mut()
            .expect("owned child already taken")
            .handoff()?;
        process.handed_off = true;
    }
    let exit = child.exit.clone();
    if let Some(cleanup) = child.cleanup.take() {
        let _ = cleanup.send(());
    }
    Ok(IndependentChild { exit })
}

#[cfg(unix)]
pub(super) struct DeferredPlan {
    ready: oneshot::Sender<()>,
    resume: oneshot::Receiver<std::ffi::OsString>,
}
#[cfg(unix)]
pub struct PreparedIndependent {
    task: Option<tokio::task::JoinHandle<std::io::Result<IndependentChild>>>,
    resume: Option<oneshot::Sender<std::ffi::OsString>>,
}
#[cfg(unix)]
impl PreparedIndependent {
    pub async fn commit(
        mut self,
        program: impl AsRef<std::ffi::OsStr>,
    ) -> std::io::Result<IndependentChild> {
        if let Some(resume) = self.resume.take() {
            let _ = resume.send(program.as_ref().to_owned());
        }
        self.result().await
    }
    async fn result(&mut self) -> std::io::Result<IndependentChild> {
        let task = self
            .task
            .as_mut()
            .ok_or_else(|| std::io::Error::other("prepared launcher already consumed"))?;
        let result = task
            .await
            .map_err(|_| std::io::Error::other("prepared launch owner ended without a result"))?;
        self.task.take();
        result
    }
}
#[cfg(unix)]
impl Drop for PreparedIndependent {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}
#[cfg(unix)]
impl GroupCommand {
    /// Pin a running helper before an updater can replace its on-disk image.
    pub async fn prepare_independent(
        mut self,
        purpose: IndependentProcess,
    ) -> std::io::Result<PreparedIndependent> {
        if self
            .stdio
            .iter()
            .any(|mode| matches!(mode, Some(ProcessStdio::Piped)))
        {
            return Err(std::io::Error::other(
                "independent process cannot own caller pipes",
            ));
        }
        self.independent = Some(purpose);
        self.session = SessionMode::New;
        let (ready_tx, ready_rx) = oneshot::channel();
        let (resume_tx, resume_rx) = oneshot::channel();
        let task = spawn(async move {
            finish_handoff(
                launch(
                    self,
                    Some(DeferredPlan {
                        ready: ready_tx,
                        resume: resume_rx,
                    }),
                )
                .await?,
            )
        });
        let mut prepared = PreparedIndependent {
            task: Some(task),
            resume: Some(resume_tx),
        };
        if ready_rx.await.is_err() {
            prepared.result().await?;
            return Err(std::io::Error::other(
                "helper completed without preparation acknowledgement",
            ));
        }
        Ok(prepared)
    }
}
