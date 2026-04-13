use serde::{Deserialize, Serialize};
use tokio::fs;

#[derive(Serialize, Deserialize)]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub is_directory: bool,
    pub size: Option<u64>,
}

#[derive(Serialize, Deserialize)]
pub struct FileContent {
    pub path: String,
    pub content: String,
    pub size: u64,
    pub truncated: bool,
}

fn ensure_local_path(path: &str, op: &str) -> Result<(), String> {
    if path.trim().starts_with("ssh://") {
        return Err(format!(
            "{op} requires a local path; remote paths must be handled by a Tyde server connection"
        ));
    }
    Ok(())
}

pub async fn list_directory(path: &str, show_hidden: bool) -> Result<Vec<FileEntry>, String> {
    ensure_local_path(path, "list_directory")?;

    let mut reader = fs::read_dir(path).await.map_err(|e| format!("{e:?}"))?;
    let mut entries = Vec::new();

    while let Some(entry) = reader.next_entry().await.map_err(|e| format!("{e:?}"))? {
        let name = entry.file_name().to_string_lossy().to_string();

        if !show_hidden
            && (name.starts_with('.')
                || ["node_modules", "target", ".git"].contains(&name.as_str()))
        {
            continue;
        }

        let metadata = entry.metadata().await.map_err(|e| format!("{e:?}"))?;
        let is_directory = metadata.is_dir();
        let size = if is_directory {
            None
        } else {
            Some(metadata.len())
        };

        entries.push(FileEntry {
            name,
            path: entry.path().to_string_lossy().to_string(),
            is_directory,
            size,
        });
    }

    entries.sort_by(|a, b| match (a.is_directory, b.is_directory) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });

    Ok(entries)
}

pub async fn read_file_content(path: &str) -> Result<FileContent, String> {
    ensure_local_path(path, "read_file_content")?;

    let metadata = fs::metadata(path).await.map_err(|e| format!("{e:?}"))?;
    let size = metadata.len();

    let bytes = fs::read(path).await.map_err(|e| format!("{e:?}"))?;

    let check_len = bytes.len().min(512);
    if bytes[..check_len].contains(&0x00) {
        return Ok(FileContent {
            path: path.to_string(),
            content: "Binary file".to_string(),
            size,
            truncated: false,
        });
    }

    let truncated = size > 1_048_576;
    let usable = if truncated {
        &bytes[..1_048_576_usize]
    } else {
        &bytes
    };

    let content = String::from_utf8_lossy(usable).to_string();

    Ok(FileContent {
        path: path.to_string(),
        content,
        size,
        truncated,
    })
}
