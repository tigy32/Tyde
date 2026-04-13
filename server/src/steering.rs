use std::path::PathBuf;

pub fn write_codex_steering_tempfile(content: &str) -> Result<PathBuf, String> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Err("Steering content must not be empty".to_string());
    }

    let filename = format!("tyde-codex-steering-{}.md", uuid::Uuid::new_v4());
    let path = std::env::temp_dir().join(filename);
    std::fs::write(&path, trimmed)
        .map_err(|err| format!("Failed to write codex steering tempfile: {err}"))?;
    Ok(path)
}
