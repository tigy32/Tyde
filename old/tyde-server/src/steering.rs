use std::path::{Path, PathBuf};
use tokio::fs;

/// Read steering docs from `~/.tyde/steering/*.md` (global) then
/// `{workspace_root}/.tyde/steering/*.md` (per-workspace), concatenated
/// with blank-line separators.  Returns `Ok(None)` if no steering files exist.
pub async fn read_steering(workspace_root: &str) -> Result<Option<String>, String> {
    let mut parts: Vec<String> = Vec::new();

    // Global steering: ~/.tyde/steering/*.md
    if let Ok(home) = std::env::var("HOME") {
        let global_dir = Path::new(&home).join(".tyde").join("steering");
        collect_md_files(&global_dir, &mut parts).await?;
    }

    // Per-workspace steering: {workspace_root}/.tyde/steering/*.md
    let workspace_dir = Path::new(workspace_root).join(".tyde").join("steering");
    collect_md_files(&workspace_dir, &mut parts).await?;

    if parts.is_empty() {
        Ok(None)
    } else {
        Ok(Some(parts.join("\n\n")))
    }
}

/// Convenience: pick the first local workspace root from the slice and read steering.
pub async fn read_steering_from_roots(
    workspace_roots: &[String],
) -> Result<Option<String>, String> {
    let root = match workspace_roots
        .iter()
        .find(|r| !r.trim().is_empty() && !r.starts_with("ssh://"))
    {
        Some(r) => r,
        None => return Ok(None),
    };
    read_steering(root).await
}

/// Write steering content to a temp file and return the path.
pub fn write_codex_steering_tempfile(content: &str) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir();
    let id = std::process::id();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = dir.join(format!("tyde-steering-{id}-{ts}.md"));
    std::fs::write(&path, content)
        .map_err(|e| format!("Failed to write steering temp file: {e}"))?;
    Ok(path)
}

/// Create a temporary workspace root containing `.tycode/*.md` steering so the
/// Tycode subprocess can ingest Tyde steering content through its native loader.
pub fn write_tycode_steering_root(content: &str) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir();
    let id = std::process::id();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let root = dir.join(format!("tyde-tycode-steering-{id}-{ts}"));
    let tycode_dir = root.join(".tycode");
    std::fs::create_dir_all(&tycode_dir)
        .map_err(|e| format!("Failed to create Tycode steering dir: {e}"))?;
    let path = tycode_dir.join("tyde_steering.md");
    std::fs::write(&path, content).map_err(|e| format!("Failed to write Tycode steering: {e}"))?;
    Ok(root)
}

async fn collect_md_files(dir: &Path, parts: &mut Vec<String>) -> Result<(), String> {
    let mut entries = match fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(format!(
                "Failed to read steering directory {}: {e}",
                dir.display()
            ))
        }
    };
    let mut paths: Vec<PathBuf> = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("md") {
            paths.push(path);
        }
    }
    paths.sort();
    for path in paths {
        let content = fs::read_to_string(&path)
            .await
            .map_err(|e| format!("Failed to read steering file {}: {e}", path.display()))?;
        let trimmed = content.trim();
        if !trimmed.is_empty() {
            parts.push(trimmed.to_string());
        }
    }
    Ok(())
}
