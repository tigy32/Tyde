use tokio::process::Command;

#[derive(Clone, Debug, Default)]
pub(crate) enum BackendTransport {
    #[default]
    Local,
    Ssh {
        host: String,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct BackendLaunchTarget {
    pub(crate) transport: BackendTransport,
    pub(crate) executable_path: String,
}

impl BackendTransport {
    pub(crate) fn from_ssh_host(ssh_host: Option<String>) -> Self {
        match ssh_host {
            Some(host) => Self::Ssh { host },
            None => Self::Local,
        }
    }

    pub(crate) fn is_remote(&self) -> bool {
        matches!(self, Self::Ssh { .. })
    }

    pub(crate) fn ssh_host(&self) -> Option<&str> {
        match self {
            Self::Local => None,
            Self::Ssh { host } => Some(host.as_str()),
        }
    }

    pub(crate) async fn spawn_process(
        &self,
        program: &str,
        args: &[String],
        cwd: Option<&str>,
    ) -> Result<tokio::process::Child, String> {
        crate::remote::spawn_local_or_remote_process(self.ssh_host(), program, args, cwd).await
    }

    pub(crate) async fn run_shell_command(
        &self,
        command: &str,
    ) -> Result<std::process::Output, String> {
        match self {
            Self::Local => Command::new("sh")
                .arg("-lc")
                .arg(command)
                .output()
                .await
                .map_err(|e| format!("Failed to run local shell command: {e}")),
            Self::Ssh { host } => crate::remote::run_ssh_raw(host, command).await,
        }
    }
}

impl BackendLaunchTarget {
    pub(crate) fn local(executable_path: impl Into<String>) -> Self {
        Self {
            transport: BackendTransport::Local,
            executable_path: executable_path.into(),
        }
    }

    pub(crate) fn remote(host: impl Into<String>, executable_path: impl Into<String>) -> Self {
        Self {
            transport: BackendTransport::Ssh { host: host.into() },
            executable_path: executable_path.into(),
        }
    }
}
