use std::path::PathBuf;

pub(crate) const SSH_WORKSPACE_ROOTS_REMOVED: &str = "SSH workspace roots are no longer supported. Connect to the Tyde server that owns the project and use its filesystem paths.";

pub(crate) fn reject_ssh_workspace_roots(
    roots: impl IntoIterator<Item = impl AsRef<str>>,
) -> Result<(), String> {
    if roots.into_iter().any(|root| {
        root.as_ref()
            .trim_start()
            .get(.."ssh://".len())
            .is_some_and(|scheme| scheme.eq_ignore_ascii_case("ssh://"))
    }) {
        return Err(SSH_WORKSPACE_ROOTS_REMOVED.to_owned());
    }
    Ok(())
}

pub fn home_dir() -> Result<PathBuf, String> {
    home_dir_from_env(|name| std::env::var(name).ok())
}

pub(crate) fn execution_home_dir() -> Result<PathBuf, String> {
    home_dir_from_env(|name| tyde_process_env::launch::environment_var(name).ok())
}

fn home_dir_from_env<F>(mut env: F) -> Result<PathBuf, String>
where
    F: FnMut(&str) -> Option<String>,
{
    if let Some(home) = non_empty_env_path(&mut env, "HOME") {
        return Ok(home);
    }

    if let Some(home) = non_empty_env_path(&mut env, "USERPROFILE") {
        return Ok(home);
    }

    match (
        non_empty_env_value(&mut env, "HOMEDRIVE"),
        non_empty_env_value(&mut env, "HOMEPATH"),
    ) {
        (Some(drive), Some(path)) => Ok(PathBuf::from(format!("{drive}{path}"))),
        _ => Err("Cannot determine home directory".to_string()),
    }
}

fn non_empty_env_path<F>(env: &mut F, name: &str) -> Option<PathBuf>
where
    F: FnMut(&str) -> Option<String>,
{
    non_empty_env_value(env, name).map(PathBuf::from)
}

fn non_empty_env_value<F>(env: &mut F, name: &str) -> Option<String>
where
    F: FnMut(&str) -> Option<String>,
{
    env(name)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}
