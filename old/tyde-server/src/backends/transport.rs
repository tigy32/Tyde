use tokio::process::Command;

#[derive(Clone, Debug, Default)]
pub enum BackendTransport {
    #[default]
    Local,
}

#[derive(Clone, Debug)]
pub struct BackendLaunchTarget {
    pub transport: BackendTransport,
    pub executable_path: String,
}

impl BackendTransport {
    pub async fn spawn_process(
        &self,
        program: &str,
        args: &[String],
        cwd: Option<&str>,
    ) -> Result<tokio::process::Child, String> {
        let mut command = Command::new(program);
        command.args(args);
        if let Some(dir) = cwd.map(str::trim).filter(|dir| !dir.is_empty()) {
            command.current_dir(dir);
        }
        command.stdin(std::process::Stdio::piped());
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());
        command
            .spawn()
            .map_err(|e| format!("Failed to spawn local process '{program}': {e}"))
    }

    pub async fn run_shell_command(&self, command: &str) -> Result<std::process::Output, String> {
        Command::new("sh")
            .arg("-lc")
            .arg(command)
            .output()
            .await
            .map_err(|e| format!("Failed to run local shell command: {e}"))
    }
}

impl BackendLaunchTarget {
    pub fn local(executable_path: impl Into<String>) -> Self {
        Self {
            transport: BackendTransport::Local,
            executable_path: executable_path.into(),
        }
    }
}
