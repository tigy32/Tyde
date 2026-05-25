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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use super::home_dir_from_env;

    #[test]
    fn uses_home_when_available() {
        assert_eq!(
            home_dir_from_env(test_env(&[
                ("HOME", "/Users/test"),
                ("USERPROFILE", "C:\\Users\\test")
            ]))
            .unwrap(),
            PathBuf::from("/Users/test")
        );
    }

    #[test]
    fn falls_back_to_userprofile() {
        assert_eq!(
            home_dir_from_env(test_env(&[("USERPROFILE", "C:\\Users\\test")])).unwrap(),
            PathBuf::from("C:\\Users\\test")
        );
    }

    #[test]
    fn falls_back_to_homedrive_and_homepath() {
        assert_eq!(
            home_dir_from_env(test_env(&[
                ("HOMEDRIVE", "D:"),
                ("HOMEPATH", "\\Users\\test")
            ]))
            .unwrap(),
            PathBuf::from("D:\\Users\\test")
        );
    }

    #[test]
    fn ignores_empty_values() {
        assert_eq!(
            home_dir_from_env(test_env(&[
                ("HOME", " "),
                ("USERPROFILE", ""),
                ("HOMEDRIVE", "H:"),
                ("HOMEPATH", "\\Users\\test"),
            ]))
            .unwrap(),
            PathBuf::from("H:\\Users\\test")
        );
    }

    fn test_env(values: &[(&str, &str)]) -> impl FnMut(&str) -> Option<String> {
        let values = values
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect::<HashMap<_, _>>();
        move |name| values.get(name).cloned()
    }
}
