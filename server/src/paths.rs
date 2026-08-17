use std::path::PathBuf;

pub fn home_dir() -> Result<PathBuf, String> {
    home_dir_from_env(|name| std::env::var(name).ok())
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
