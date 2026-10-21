use std::path::Path;
#[cfg(target_os = "linux")]
use std::sync::Arc;

#[cfg(target_os = "linux")]
use super::AsyncGroupChild;
use super::{AsyncCommandGroup, GroupCommand, ProcessStdio};

#[derive(Clone, Debug, Default)]
pub struct ExecutionContext {
    #[cfg(target_os = "linux")]
    instance: Option<Arc<Instance>>,
}

#[derive(Clone, Copy, Debug)]
pub enum ExecutionRoute {
    NativeHost,
    CallerInstance,
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct Instance {
    directory: std::fs::File,
    python: std::path::PathBuf,
}

impl ExecutionContext {
    pub fn route(&self) -> ExecutionRoute {
        if self.is_native() {
            ExecutionRoute::NativeHost
        } else {
            ExecutionRoute::CallerInstance
        }
    }

    pub fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Result<GroupCommand, String> {
        #[cfg(target_os = "linux")]
        if self.instance.is_some() {
            let mut command = super::raw_command(program);
            command.active_execution = Some(self.clone());
            return Ok(command);
        }
        super::host_command(program)
    }

    fn is_native(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.instance.is_none()
        }
        #[cfg(not(target_os = "linux"))]
        {
            true
        }
    }

    fn filesystem_command(&self, script: &str, path: &Path) -> std::io::Result<GroupCommand> {
        let python = crate::find_host_executable_in_path("python3")
            .ok_or_else(|| std::io::Error::other("Execution filesystem requires host Python 3"))?;
        let mut command = self.command(python).map_err(std::io::Error::other)?;
        command.args(["-c", script]).arg(path);
        Ok(command)
    }

    pub async fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        if self.is_native() {
            return tokio::fs::read(path).await;
        }
        let mut command = self.filesystem_command(
            "import pathlib,sys; sys.stdout.buffer.write(pathlib.Path(sys.argv[1]).read_bytes())",
            path,
        )?;
        let output = command.output().await?;
        if !output.status.success() {
            return Err(std::io::Error::other(format!(
                "Execution filesystem read failed ({})",
                output.status
            )));
        }
        Ok(output.stdout)
    }

    pub async fn write(&self, path: &Path, contents: &[u8]) -> std::io::Result<()> {
        if self.is_native() {
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            return tokio::fs::write(path, contents).await;
        }
        let mut command = self.filesystem_command(
            "import pathlib,sys; p=pathlib.Path(sys.argv[1]); p.parent.mkdir(parents=True, exist_ok=True); p.write_bytes(sys.stdin.buffer.read())", path,
        )?;
        command
            .stdin(ProcessStdio::Piped)
            .stdout(ProcessStdio::Piped)
            .stderr(ProcessStdio::Piped);
        let mut child = command.group_spawn().await?;
        let mut input = child
            .take_stdin()
            .ok_or_else(|| std::io::Error::other("Execution filesystem input is unavailable"))?;
        tokio::io::AsyncWriteExt::write_all(&mut input, contents).await?;
        drop(input);
        let output = child.wait_with_output().await?;
        if !output.status.success() {
            return Err(std::io::Error::other(format!(
                "Execution filesystem write failed ({})",
                output.status
            )));
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    pub(super) fn wrap(&self, mut command: GroupCommand) -> std::io::Result<GroupCommand> {
        use std::io::{Seek, Write};
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::ffi::OsStrExt;
        #[derive(serde::Serialize)]
        struct Plan {
            command: Vec<Vec<u8>>,
            cwd: Option<Vec<u8>>,
            environment: Vec<(Vec<u8>, Option<Vec<u8>>)>,
        }
        let instance = self
            .instance
            .as_ref()
            .ok_or_else(|| std::io::Error::other("Execution instance is absent"))?;
        let directory = std::path::PathBuf::from(format!(
            "/proc/{}/fd/{}",
            std::process::id(),
            instance.directory.as_raw_fd()
        ));
        let mut argv = vec![command.program.as_bytes().to_vec()];
        argv.extend(command.args.iter().map(|arg| arg.as_bytes().to_vec()));
        let plan = Plan {
            command: argv,
            cwd: command
                .cwd
                .as_ref()
                .map(|path| path.as_os_str().as_bytes().to_vec()),
            environment: command
                .env
                .iter()
                .map(|(key, value)| {
                    (
                        key.as_bytes().to_vec(),
                        value.as_ref().map(|value| value.as_bytes().to_vec()),
                    )
                })
                .collect(),
        };
        if command.independent.is_some() {
            return Err(std::io::Error::other(
                "Active execution commands must remain owned by their instance",
            ));
        }
        let descriptor = unsafe {
            libc::memfd_create(
                c"tyde-execution-plan".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        if descriptor < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut plan_file = unsafe { std::fs::File::from_raw_fd(descriptor) };
        plan_file.write_all(&serde_json::to_vec(&plan).map_err(std::io::Error::other)?)?;
        plan_file.rewind()?;
        if unsafe {
            libc::fcntl(
                plan_file.as_raw_fd(),
                libc::F_ADD_SEALS,
                libc::F_SEAL_SEAL | libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK,
            )
        } < 0
        {
            return Err(std::io::Error::last_os_error());
        }
        let plan_path = format!("/proc/{}/fd/{}", std::process::id(), plan_file.as_raw_fd());
        command.execution_plan = Some(Arc::new(plan_file));
        command.program = instance.python.as_os_str().to_owned();
        command.args = vec![
            directory.join("runner.py").into_os_string(),
            "execute".into(),
            directory.join("execution.sock").into_os_string(),
            plan_path.into(),
        ];
        command.env.clear();
        command.cwd = None;
        command.workbench = None;
        command.prepared_environment = None;
        Ok(command)
    }
}

#[cfg(target_os = "linux")]
pub(super) struct Setup {
    directory: tempfile::TempDir,
    listener: tokio::net::UnixListener,
    runs: std::path::PathBuf,
    python: std::path::PathBuf,
}

#[cfg(target_os = "linux")]
impl Setup {
    pub(super) fn new(manifest: &Path, python: std::path::PathBuf) -> std::io::Result<Self> {
        let directory = tempfile::Builder::new()
            .prefix("tyde-execution-")
            .tempdir_in("/tmp")?;
        let listener = tokio::net::UnixListener::bind(directory.path().join("ready.sock"))?;
        let runs = manifest
            .parent()
            .ok_or_else(|| std::io::Error::other("Execution manifest has no owner directory"))?
            .join("runs");
        Ok(Self {
            directory,
            listener,
            runs,
            python,
        })
    }

    pub(super) fn address(&self) -> std::path::PathBuf {
        self.directory.path().join("ready.sock")
    }

    pub(super) async fn receive(
        self,
        child: &mut AsyncGroupChild,
    ) -> std::io::Result<ExecutionContext> {
        use std::os::unix::ffi::OsStringExt;
        use std::os::unix::fs::OpenOptionsExt;
        use tokio::io::AsyncReadExt;
        let (mut stream, _) = tokio::select! {
            connection = self.listener.accept() => connection?,
            result = child.wait() => return Err(std::io::Error::other(format!("Native process closed before its execution instance was established ({})", result?))),
        };
        let mut length = [0; 8];
        stream.read_exact(&mut length).await?;
        let length = usize::try_from(u64::from_be_bytes(length)).map_err(std::io::Error::other)?;
        if length > 1024 * 1024 {
            return Err(std::io::Error::other(
                "Execution instance descriptor exceeds its protocol limit",
            ));
        }
        let mut path = vec![0; length];
        stream.read_exact(&mut path).await?;
        let path = std::path::PathBuf::from(std::ffi::OsString::from_vec(path));
        if path.parent() != Some(self.runs.as_path()) {
            return Err(std::io::Error::other(
                "Execution instance does not belong to its launch",
            ));
        }
        let directory = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        Ok(ExecutionContext {
            instance: Some(Arc::new(Instance {
                directory,
                python: self.python,
            })),
        })
    }
}
